//! Where an item stands and what is happening to it, as tool output carries
//! them: the reading the board makes of the record, at the hub's clock rather
//! than a replay's.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::Value;

use super::runs::text;
use super::timings::parse_at;
use super::{history, placements, PipelineDefinition, Tone, NO_DETAIL};

/// How long a claim holds when its rule names a lease field the event does
/// not carry, as claims recorded before leases were.
pub const LEGACY_LEASE_MINUTES: f64 = 45.0;

/// The column an item is in, as its latest placing event put it there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Position {
    pub column: String,
    pub tone: Tone,
    pub label: String,
    /// When the placing event was recorded.
    pub at: String,
    /// The annotation noted on the item since it last moved.
    pub annotation: Option<Annotation>,
}

/// An annotation standing on an item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Annotation {
    pub tone: Tone,
    pub label: String,
    pub at: String,
}

/// What is happening to an item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Activity {
    /// A run claimed it at `since` and holds it `until` its lease runs out,
    /// or until it moves when its rule names no lease.
    Working {
        since: String,
        until: Option<DateTime<Utc>>,
    },
    /// It is next due at `due`: the time its event names, or its lapsed
    /// claim's.
    Scheduled {
        due: DateTime<Utc>,
    },
    Other,
}

/// Where `body` stands per `def`; `None` before any event places it.
pub fn position(def: &PipelineDefinition, body: &Value) -> Option<Position> {
    placements(def, body).last().map(|placement| Position {
        column: placement.column.to_string(),
        tone: placement.rule.tone.unwrap_or_default(),
        label: def.label_of(
            placement.rule,
            placement.stage,
            placement.event,
            placement.detail,
        ),
        at: placement.at.to_string(),
        annotation: standing_annotation(def, body),
    })
}

/// The newest annotation since the item last moved: a placement clears it,
/// and an event the definition has no rule for leaves it standing.
pub fn standing_annotation(def: &PipelineDefinition, body: &Value) -> Option<Annotation> {
    history(body).fold(None, |standing, event| {
        let stage = text(event, "stage").unwrap_or_default();
        let name = text(event, "event").unwrap_or_default();
        let detail = event.get("detail").unwrap_or(&NO_DETAIL);
        match def.rule_for(stage, name, detail) {
            Some(rule) if rule.annotation => Some(Annotation {
                tone: rule.tone.unwrap_or_default(),
                label: def.label_of(rule, stage, name, detail),
                at: text(event, "at").unwrap_or_default().to_string(),
            }),
            Some(rule) if rule.places().is_some() => None,
            _ => standing,
        }
    })
}

/// What is happening to the item at `now`, read from its latest placing
/// event's rule: a claim within its lease is work, a claim past it is due
/// from when it was claimed, and a due time the event names is when it is
/// due.
pub fn activity(def: &PipelineDefinition, body: &Value, now: DateTime<Utc>) -> Activity {
    let Some(last) = placements(def, body).last() else {
        return Activity::Other;
    };
    let rule = last.rule;
    if rule.claim {
        let since = parse_at(last.at);
        let until = rule
            .lease_field
            .as_deref()
            .zip(since)
            .map(|(field, since)| since + lease(last.detail.get(field)));
        return match since.zip(until) {
            Some((since, until)) if until <= now => Activity::Scheduled { due: since },
            _ => Activity::Working {
                since: last.at.to_string(),
                until,
            },
        };
    }
    rule.due_field
        .as_deref()
        .and_then(|field| last.detail.get(field)?.as_str())
        .and_then(parse_at)
        .map_or(Activity::Other, |due| Activity::Scheduled { due })
}

/// A claim's lease: the minutes its event records, a number or numeric
/// text, else [`LEGACY_LEASE_MINUTES`].
fn lease(recorded: Option<&Value>) -> Duration {
    let minutes = match recorded {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|minutes: &f64| minutes.is_finite())
    .unwrap_or(LEGACY_LEASE_MINUTES);
    Duration::milliseconds((minutes * 60_000.0) as i64)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::pipeline::fixtures::{ord_2797, orders, record_with};
    use crate::pipeline::Rule;

    fn at(s: &str) -> DateTime<Utc> {
        parse_at(s).unwrap()
    }

    /// The example pipeline as the board's test fixture times it: claims on implementing,
    /// reviewing and verifying (the last leased), due times on rungs and
    /// self-healing inconclusives, and an overlap annotation.
    fn timed() -> PipelineDefinition {
        let mut def = orders();
        let rules = &mut def.events;
        for key in ["implement/claimed", "review/claimed"] {
            rules.get_mut(key).unwrap()[0].claim = true;
        }
        let started = &mut rules.get_mut("verify/started").unwrap()[0];
        started.claim = true;
        started.lease_field = Some("lease_minutes".into());
        for rule in rules.get_mut("verify/rung").unwrap() {
            rule.due_field = Some("due_at".into());
        }
        rules
            .get_mut("verify/inconclusive")
            .unwrap()
            .last_mut()
            .unwrap()
            .due_field = Some("due_at".into());
        rules.insert(
            "review/overlap-judged".into(),
            vec![Rule {
                annotation: true,
                tone: Some(Tone::Warn),
                label_field: Some("headline".into()),
                ..Default::default()
            }],
        );
        def
    }

    fn body(events: &[(&str, &str, Value)]) -> Value {
        let detailed: Vec<_> = events
            .iter()
            .map(|(at, key, detail)| {
                let (stage, name) = key.split_once('/').unwrap();
                (*at, stage, name, None, detail.clone())
            })
            .collect();
        record_with("ORD-1", &detailed).body
    }

    const STARTED: &str = "2026-09-15T04:48:00Z";

    fn started(lease_minutes: Value) -> (&'static str, &'static str, Value) {
        (
            STARTED,
            "verify/started",
            json!({"lease_minutes": lease_minutes, "rung": "1h"}),
        )
    }

    #[test]
    fn a_claim_holds_while_its_lease_runs_then_falls_due_from_when_it_was_claimed() {
        let def = timed();
        let working = |lease: Value, now: &str| activity(&def, &body(&[started(lease)]), at(now));
        assert_eq!(
            working(json!(150), "2026-09-15T05:00:00Z"),
            Activity::Working {
                since: STARTED.into(),
                until: Some(at("2026-09-15T07:18:00Z"))
            }
        );
        assert!(matches!(
            working(json!("150"), "2026-09-15T07:17:59Z"),
            Activity::Working { .. }
        ));
        assert_eq!(
            working(json!(150), "2026-09-15T07:18:00Z"),
            Activity::Scheduled { due: at(STARTED) }
        );
    }

    #[test]
    fn a_claim_recording_no_lease_holds_45_minutes_and_one_under_no_lease_rule_until_it_moves() {
        let def = timed();
        for detail in [
            json!({"rung": "24h"}),
            json!({"rung": "24h", "lease_minutes": "soon"}),
        ] {
            let legacy = body(&[("2026-09-13T16:33:05Z", "verify/started", detail)]);
            assert_eq!(
                activity(&def, &legacy, at("2026-09-13T17:00:00Z")),
                Activity::Working {
                    since: "2026-09-13T16:33:05Z".into(),
                    until: Some(at("2026-09-13T17:18:05Z"))
                }
            );
            assert_eq!(
                activity(&def, &legacy, at("2026-09-15T04:55:00Z")),
                Activity::Scheduled {
                    due: at("2026-09-13T16:33:05Z")
                }
            );
        }
        let claimed = body(&[("2026-09-14T13:00:03Z", "implement/claimed", json!({}))]);
        assert_eq!(
            activity(&def, &claimed, at("2026-09-20T00:00:00Z")),
            Activity::Working {
                since: "2026-09-14T13:00:03Z".into(),
                until: None
            }
        );
    }

    #[test]
    fn a_due_time_schedules_the_item_and_a_missing_or_unparseable_one_does_not() {
        let def = timed();
        let now = at("2026-09-15T04:55:00Z");
        let overdue = body(&[(
            "2026-09-15T04:35:36Z",
            "verify/rung",
            json!({"state": "pass", "due_at": "2026-09-14T21:50:07Z"}),
        )]);
        assert_eq!(
            activity(&def, &overdue, now),
            Activity::Scheduled {
                due: at("2026-09-14T21:50:07Z")
            }
        );
        let later = body(&[(
            "2026-09-14T10:23:54Z",
            "verify/inconclusive",
            json!({"final": "false", "due_at": "2026-09-16T10:07:00Z"}),
        )]);
        assert_eq!(
            activity(&def, &later, now),
            Activity::Scheduled {
                due: at("2026-09-16T10:07:00Z")
            }
        );
        for events in [
            [(
                "2026-09-13T16:38:00Z",
                "verify/inconclusive",
                json!({"final": "false"}),
            )],
            [(
                "2026-09-13T16:38:00Z",
                "verify/inconclusive",
                json!({"final": "false", "due_at": "tomorrow"}),
            )],
            [(
                "2026-09-13T16:38:00Z",
                "verify/inconclusive",
                json!({"final": "true", "due_at": "2026-09-16T10:07:00Z"}),
            )],
            [(
                "2026-09-13T16:38:00Z",
                "merge/unknown",
                json!({"due_at": "2026-09-16T10:07:00Z"}),
            )],
        ] {
            assert_eq!(activity(&def, &body(&events), now), Activity::Other);
        }
    }

    #[test]
    fn a_claim_ends_at_the_next_placement_not_at_an_unruled_event_or_an_annotation() {
        let def = timed();
        let now = at("2026-09-14T18:00:00Z");
        let claimed = ("2026-09-14T17:23:05Z", "review/claimed", json!({}));
        let follow_ups = ("2026-09-14T17:48:25Z", "review/follow-ups", json!({}));
        let judged = (
            "2026-09-14T17:48:26Z",
            "review/overlap-judged",
            json!({"headline": "#1698 and #1702 change the same code"}),
        );
        let working = body(&[claimed.clone(), follow_ups.clone(), judged.clone()]);
        assert!(matches!(
            activity(&def, &working, now),
            Activity::Working { .. }
        ));
        let finished = (
            "2026-09-14T17:48:27Z",
            "review/finished",
            json!({"verdict": "stage-reviewed"}),
        );
        assert_eq!(
            activity(&def, &body(&[claimed, follow_ups, judged, finished]), now),
            Activity::Other
        );
    }

    #[test]
    fn position_carries_the_placing_label_and_the_annotation_standing_since_the_item_moved() {
        let def = timed();
        let item = ord_2797();
        let placed = position(&def, &item.body).unwrap();
        let last = placements(&def, &item.body).last().unwrap();
        assert_eq!(
            placed,
            Position {
                column: "verifying".into(),
                tone: last.rule.tone.unwrap_or_default(),
                label: def.label_of(last.rule, last.stage, last.event, last.detail),
                at: "2026-09-15T04:53:20.286220Z".into(),
                annotation: None,
            }
        );

        let judged = (
            "2026-09-15T05:00:00Z",
            "review/overlap-judged",
            json!({"headline": "#1698 and #1702\nchange the same code"}),
        );
        let unruled = ("2026-09-15T05:01:00Z", "review/follow-ups", json!({}));
        let standing = body(&[started(json!(150)), judged.clone(), unruled]);
        assert_eq!(
            position(&def, &standing).unwrap().annotation,
            Some(Annotation {
                tone: Tone::Warn,
                label: "#1698 and #1702 change the same code".into(),
                at: "2026-09-15T05:00:00Z".into()
            })
        );
        let moved = body(&[
            started(json!(150)),
            judged,
            (
                "2026-09-15T05:02:00Z",
                "verify/rung",
                json!({"state": "pass", "rung": "1h"}),
            ),
        ]);
        assert_eq!(position(&def, &moved).unwrap().annotation, None);

        let only_noted = body(&[(
            "2026-09-15T05:00:00Z",
            "review/overlap-judged",
            json!({"headline": "noted"}),
        )]);
        assert_eq!(position(&def, &only_noted), None);
        assert!(standing_annotation(&def, &only_noted).is_some());
        assert_eq!(
            activity(&def, &only_noted, at("2026-09-15T05:00:00Z")),
            Activity::Other
        );
    }

    #[test]
    fn position_and_activity_serialise_as_the_board_reads_them() {
        let def = timed();
        let standing = body(&[started(json!(150))]);
        assert_eq!(
            serde_json::to_value(activity(&def, &standing, at("2026-09-15T05:00:00Z"))).unwrap(),
            json!({"kind": "working", "since": STARTED, "until": "2026-09-15T07:18:00Z"})
        );
        assert_eq!(
            serde_json::to_value(activity(&def, &standing, at("2026-09-15T08:00:00Z"))).unwrap(),
            json!({"kind": "scheduled", "due": "2026-09-15T04:48:00Z"})
        );
        assert_eq!(
            serde_json::to_value(Activity::Other).unwrap(),
            json!({"kind": "other"})
        );
        let json = serde_json::to_value(position(&def, &standing)).unwrap();
        assert_eq!(json["column"], "verifying");
        assert_eq!(json["annotation"], Value::Null);
    }
}
