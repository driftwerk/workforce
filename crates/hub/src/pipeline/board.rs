//! What a board shows of a pipeline's items: every item still in play, and
//! the finished ones that entered their column within its window.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::position::standing_annotation;
use super::runs::text;
use super::timings::parse_at;
use super::{history, placements, valid_id, PipelineDefinition, Window};
use crate::store::Document;

/// A caller's choice of windows: `<window>` for every windowed column, or
/// `<column>:<window>` entries joined with `,`. The default keeps the
/// definition's windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overrides {
    Every(Window),
    Columns(BTreeMap<String, Window>),
}

impl Default for Overrides {
    fn default() -> Self {
        Overrides::Columns(BTreeMap::new())
    }
}

impl Overrides {
    fn window_for(&self, column: &str) -> Option<Window> {
        match self {
            Overrides::Every(window) => Some(*window),
            Overrides::Columns(windows) => windows.get(column).copied(),
        }
    }
}

impl std::str::FromStr for Overrides {
    type Err = String;

    fn from_str(spec: &str) -> Result<Self, String> {
        if spec.is_empty() {
            return Err("`windows` may not be empty".into());
        }
        if !spec.contains([':', ',']) {
            return spec
                .parse()
                .map(Overrides::Every)
                .map_err(|e| format!("`windows`: {e}"));
        }
        spec.split(',')
            .try_fold(BTreeMap::new(), |mut windows, entry| {
                let (column, window) = entry.split_once(':').ok_or_else(|| {
                    format!("`windows` entry `{entry}` is not `<column>:<window>`")
                })?;
                if !valid_id(column) {
                    return Err(format!(
                        "`windows` entry `{entry}` names `{column}`, which is not a column id"
                    ));
                }
                let window = window
                    .parse()
                    .map_err(|e| format!("`windows` entry `{entry}`: {e}"))?;
                match windows.insert(column.to_string(), window) {
                    Some(_) => Err(format!("`windows` names `{column}` more than once")),
                    None => Ok(windows),
                }
            })
            .map(Overrides::Columns)
    }
}

/// The window a column applied, and how many of its items it left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ColumnWindow {
    pub window: Window,
    /// The definition's window for the column.
    pub default: Window,
    pub hidden: usize,
}

/// The earliest and latest moments in any item's history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct HistorySpan {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

#[derive(Debug)]
pub struct BoardView<'a> {
    /// The records the board shows, in the order they were read.
    pub items: Vec<&'a Document>,
    /// Every windowed column, by id.
    pub windows: BTreeMap<String, ColumnWindow>,
    /// Across every record, hidden ones included; `None` when no event's
    /// time parses.
    pub span: Option<HistorySpan>,
}

/// What a board shows at `now`. A record is left out only when its last
/// placement put it in a windowed column before the window began and no
/// annotation that needs a person stands on it; an item that cannot be dated
/// is shown.
pub fn board_view<'a>(
    def: &PipelineDefinition,
    records: &'a [Document],
    overrides: &Overrides,
    now: DateTime<Utc>,
) -> BoardView<'a> {
    let applied: BTreeMap<&str, (Window, Window)> = def
        .lanes
        .iter()
        .chain(&def.exits)
        .filter_map(|column| {
            let default = column.window?;
            let window = overrides.window_for(&column.id).unwrap_or(default);
            Some((column.id.as_str(), (window, default)))
        })
        .collect();
    let hidden_in = |doc: &Document| {
        let last = placements(def, &doc.body).last()?;
        let (&column, &(window, _)) = applied.get_key_value(last.column)?;
        let cutoff = now - window.duration()?;
        let entered = parse_at(last.at)?;
        let hidden = entered < cutoff
            && !standing_annotation(def, &doc.body)
                .is_some_and(|annotation| annotation.tone.needs_person());
        hidden.then_some(column)
    };
    let (items, hidden) = records.iter().fold(
        (Vec::new(), BTreeMap::<&str, usize>::new()),
        |(mut items, mut hidden), doc| {
            match hidden_in(doc) {
                Some(column) => *hidden.entry(column).or_default() += 1,
                None => items.push(doc),
            }
            (items, hidden)
        },
    );
    let windows = applied
        .iter()
        .map(|(&column, &(window, default))| {
            let hidden = hidden.get(column).copied().unwrap_or_default();
            (
                column.to_string(),
                ColumnWindow {
                    window,
                    default,
                    hidden,
                },
            )
        })
        .collect();
    let span = records
        .iter()
        .flat_map(|doc| history(&doc.body))
        .filter_map(|event| parse_at(text(event, "at")?))
        .fold(None, |span: Option<HistorySpan>, at| {
            Some(
                span.map_or(HistorySpan { from: at, to: at }, |span| HistorySpan {
                    from: span.from.min(at),
                    to: span.to.max(at),
                }),
            )
        });
    BoardView {
        items,
        windows,
        span,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, SecondsFormat};
    use serde_json::{json, Value};

    use super::*;
    use crate::pipeline::fixtures::{doc, orders, orders_r11, record_with};
    use crate::pipeline::{count, position_of, Rule, Tone};

    const NOW: &str = "2026-09-15T06:00:00Z";
    const DAY: i64 = 24;

    fn now() -> DateTime<Utc> {
        parse_at(NOW).unwrap()
    }

    fn hours_ago(hours: i64) -> String {
        (now() - Duration::hours(hours)).to_rfc3339_opts(SecondsFormat::Micros, true)
    }

    /// A record of `(hours ago, "stage/event", detail)` events.
    fn item(id: &str, events: &[(i64, &str, Value)]) -> Document {
        let ats: Vec<String> = events
            .iter()
            .map(|(hours, _, _)| hours_ago(*hours))
            .collect();
        let events: Vec<_> = events
            .iter()
            .zip(&ats)
            .map(|((_, key, detail), at)| {
                let (stage, name) = key.split_once('/').unwrap();
                (at.as_str(), stage, name, None, detail.clone())
            })
            .collect();
        record_with(id, &events)
    }

    fn verified(id: &str, hours: i64) -> Document {
        item(id, &[(hours, "verify/verified", json!({"rung": "168h"}))])
    }

    fn view<'a>(
        def: &PipelineDefinition,
        records: &'a [Document],
        overrides: &str,
    ) -> BoardView<'a> {
        let overrides = match overrides {
            "" => Overrides::default(),
            spec => spec.parse().unwrap(),
        };
        board_view(def, records, &overrides, now())
    }

    fn shown<'a>(view: &BoardView<'a>) -> Vec<&'a str> {
        view.items.iter().map(|doc| doc.doc_id.as_str()).collect()
    }

    fn window(window: Window, default: Window, hidden: usize) -> ColumnWindow {
        ColumnWindow {
            window,
            default,
            hidden,
        }
    }

    const WEEK: Window = Window::Days(7);

    #[test]
    fn a_definition_without_windows_shows_every_record() {
        let records = [
            verified("ORD-1", 30 * DAY),
            item(
                "ORD-2",
                &[(
                    40 * DAY,
                    "implement/released",
                    json!({"label": "stage-skipped"}),
                )],
            ),
            doc("ORD-3", json!({})),
        ];
        let board = view(&orders(), &records, "");
        assert_eq!(shown(&board), ["ORD-1", "ORD-2", "ORD-3"]);
        assert!(board.windows.is_empty());
        assert_eq!(
            board.span,
            Some(HistorySpan {
                from: now() - Duration::hours(40 * DAY),
                to: now() - Duration::hours(30 * DAY),
            })
        );
    }

    #[test]
    fn only_a_finished_item_older_than_its_window_is_hidden() {
        let records = [
            item(
                "STALE",
                &[(
                    30 * DAY,
                    "verify/rung",
                    json!({"state": "pass", "rung": "1h"}),
                )],
            ),
            verified("EIGHT-DAYS", 8 * DAY),
            verified("SIX-DAYS", 6 * DAY),
            verified("AT-CUTOFF", 7 * DAY),
            item("NOT-FIXED", &[(40 * DAY, "verify/failed", json!({}))]),
        ];
        let board = view(&orders_r11(), &records, "");
        assert_eq!(
            shown(&board),
            ["STALE", "SIX-DAYS", "AT-CUTOFF", "NOT-FIXED"]
        );
        assert_eq!(board.windows["verified"], window(WEEK, WEEK, 1));
        assert_eq!(board.windows["superseded"], window(WEEK, WEEK, 0));
        assert_eq!(
            board.windows.keys().collect::<Vec<_>>(),
            ["duplicate", "not-code", "superseded", "verified"]
        );
    }

    #[test]
    fn a_caller_widens_one_column_or_turns_every_window_off() {
        let def = orders_r11();
        let records = [
            verified("EIGHT-DAYS", 8 * DAY),
            item(
                "OLD-SKIP",
                &[(
                    20 * DAY,
                    "implement/released",
                    json!({"label": "stage-skipped"}),
                )],
            ),
        ];

        let widened = view(&def, &records, "verified:30d");
        assert_eq!(shown(&widened), ["EIGHT-DAYS"]);
        assert_eq!(
            widened.windows["verified"],
            window(Window::Days(30), WEEK, 0)
        );
        assert_eq!(widened.windows["not-code"], window(WEEK, WEEK, 1));

        let everything = view(&def, &records, "all");
        assert_eq!(shown(&everything), ["EIGHT-DAYS", "OLD-SKIP"]);
        assert!(everything
            .windows
            .values()
            .all(|applied| applied.window == Window::All && applied.hidden == 0));
        assert_eq!(everything.windows.len(), 4);
    }

    #[test]
    fn an_override_for_a_column_without_a_window_or_no_column_is_ignored() {
        let def = orders_r11();
        let records = [
            item("NOT-FIXED", &[(40 * DAY, "verify/failed", json!({}))]),
            verified("EIGHT-DAYS", 8 * DAY),
        ];
        let board = view(&def, &records, "not-fixed:24h,nowhere:24h");
        assert_eq!(shown(&board), ["NOT-FIXED"]);
        assert!(!board.windows.contains_key("not-fixed"));
        assert!(!board.windows.contains_key("nowhere"));
        assert_eq!(board.windows["verified"], window(WEEK, WEEK, 1));
    }

    #[test]
    fn only_a_later_placement_restarts_the_window() {
        let def = orders_r11();
        let placed = (20 * DAY, "verify/verified", json!({"rung": "168h"}));
        let records = [
            item(
                "UNRULED",
                &[placed.clone(), (DAY, "verify/checked", json!({}))],
            ),
            item("NOTED", &[placed.clone(), (DAY, "watch/stale", json!({}))]),
            item(
                "AGAIN",
                &[
                    placed.clone(),
                    (DAY, "verify/verified", json!({"rung": "168h"})),
                ],
            ),
            item(
                "BACK-IN-A-LANE",
                &[
                    placed,
                    (
                        DAY * 10,
                        "verify/rung",
                        json!({"state": "pass", "rung": "1h"}),
                    ),
                ],
            ),
        ];
        let board = view(&def, &records, "");
        assert_eq!(shown(&board), ["AGAIN", "BACK-IN-A-LANE"]);
        assert_eq!(board.windows["verified"].hidden, 2);
    }

    #[test]
    fn an_annotation_that_needs_a_person_keeps_a_finished_item_shown() {
        let mut def = orders_r11();
        def.events.insert(
            "watch/halted".into(),
            vec![Rule {
                annotation: true,
                tone: Some(Tone::Bad),
                ..Default::default()
            }],
        );
        let placed = (20 * DAY, "verify/verified", json!({"rung": "168h"}));
        let annotated = |id: &str, key: &'static str| {
            item(id, &[placed.clone(), (DAY, key, json!({"note": "n"}))])
        };
        let records = [
            annotated("WARN", "watch/escalated"),
            annotated("BAD", "watch/halted"),
            annotated("NEUTRAL", "watch/stale"),
            annotated("GOOD", "watch/resolved"),
            annotated("HEALING", "watch/realigned"),
            item(
                "CLEARED",
                &[
                    placed.clone(),
                    (19 * DAY, "watch/escalated", json!({"note": "n"})),
                    (10 * DAY, "verify/verified", json!({"rung": "168h"})),
                ],
            ),
        ];
        let board = view(&def, &records, "");
        assert_eq!(shown(&board), ["WARN", "BAD"]);
        assert_eq!(board.windows["verified"].hidden, 4);
    }

    #[test]
    fn an_item_that_cannot_be_dated_or_was_never_placed_is_shown() {
        let def = orders_r11();
        let records = [
            record_with(
                "UNPARSEABLE",
                &[("last tuesday", "verify", "verified", None, json!({}))],
            ),
            item("FUTURE", &[(-DAY, "verify/verified", json!({}))]),
            doc("EMPTY", json!({})),
            doc(
                "NO-HISTORY",
                json!({"item": "NO-HISTORY", "stage": "verify"}),
            ),
            item("UNPLACED", &[(30 * DAY, "verify/checked", json!({}))]),
        ];
        let board = view(&def, &records, "");
        assert_eq!(
            shown(&board),
            ["UNPARSEABLE", "FUTURE", "EMPTY", "NO-HISTORY", "UNPLACED"]
        );
        assert_eq!(board.windows["verified"].hidden, 0);
    }

    #[test]
    fn the_span_covers_hidden_records_ordered_by_instant() {
        let def = orders_r11();
        let at = |at: &'static str| (at, "verify", "verified", None, json!({}));
        let records = [
            record_with("HIDDEN", &[at("2026-09-01T10:30:00.000000001+02:00")]),
            record_with(
                "SHOWN",
                &[
                    at("2026-09-01T09:00:00.000001Z"),
                    at("2026-09-14T12:00:00.000000500+00:00"),
                ],
            ),
            record_with("NEWEST", &[at("2026-09-14T13:00:00.000001+01:00")]),
        ];
        let board = view(&def, &records, "");
        assert_eq!(shown(&board), ["SHOWN", "NEWEST"]);
        assert_eq!(
            board.span,
            Some(HistorySpan {
                from: parse_at("2026-09-01T08:30:00.000000001Z").unwrap(),
                to: parse_at("2026-09-14T12:00:00.000001Z").unwrap(),
            })
        );
        let undated = [
            doc("EMPTY", json!({})),
            record_with(
                "UNPARSEABLE",
                &[("soon", "verify", "verified", None, json!({}))],
            ),
        ];
        assert_eq!(view(&def, &undated, "").span, None);
    }

    #[test]
    fn overrides_read_one_window_for_every_column_or_one_per_column() {
        assert_eq!("all".parse(), Ok(Overrides::Every(Window::All)));
        assert_eq!("7d".parse(), Ok(Overrides::Every(WEEK)));
        assert_eq!(
            "verified:30d,not-code:all".parse(),
            Ok(Overrides::Columns(
                [
                    ("verified".to_string(), Window::Days(30)),
                    ("not-code".to_string(), Window::All),
                ]
                .into()
            ))
        );
        assert_eq!(
            "verified:30d".parse(),
            Ok(Overrides::Columns(
                [("verified".to_string(), Window::Days(30))].into()
            ))
        );
    }

    #[test]
    fn overrides_name_what_is_wrong() {
        for (spec, message) in [
            ("verified", "`windows`: `verified` needs a whole number before `d`"),
            ("verified:", "`windows` entry `verified:`: a window may not be empty; write `<n>h`, `<n>d` or `all`"),
            (":7d", "`windows` entry `:7d` names ``, which is not a column id"),
            ("verified:2h", "`windows` entry `verified:2h`: `2h` is shorter than 24 hours"),
            ("verified:7d,verified:30d", "`windows` names `verified` more than once"),
            ("Verified:7d", "`windows` entry `Verified:7d` names `Verified`, which is not a column id"),
            ("verified:30d,not-code", "`windows` entry `not-code` is not `<column>:<window>`"),
            ("", "`windows` may not be empty"),
        ] {
            assert_eq!(spec.parse::<Overrides>(), Err(message.to_string()), "{spec}");
        }
    }

    #[test]
    fn windows_and_span_serialise_as_the_board_reads_them() {
        let records = [verified("EIGHT-DAYS", 8 * DAY)];
        let board = view(&orders_r11(), &records, "not-code:30d");
        assert_eq!(
            serde_json::to_value(&board.windows).unwrap(),
            json!({
                "duplicate": {"window": "7d", "default": "7d", "hidden": 0},
                "not-code": {"window": "30d", "default": "7d", "hidden": 0},
                "superseded": {"window": "7d", "default": "7d", "hidden": 0},
                "verified": {"window": "7d", "default": "7d", "hidden": 1},
            })
        );
        assert_eq!(
            serde_json::to_value(board.span).unwrap(),
            json!({"from": "2026-09-07T06:00:00Z", "to": "2026-09-07T06:00:00Z"})
        );
    }

    #[test]
    fn shown_and_hidden_items_add_up_to_the_column_counts() {
        let def = orders_r11();
        let records = [
            verified("OLD-VERIFIED", 9 * DAY),
            verified("NEW-VERIFIED", DAY),
            item(
                "OLD-SKIP",
                &[(
                    12 * DAY,
                    "implement/released",
                    json!({"label": "stage-skipped"}),
                )],
            ),
            item(
                "OLD-SUPERSEDED",
                &[(21 * DAY, "merge/superseded", json!({"note": "n"}))],
            ),
            item(
                "OLD-DUPLICATE",
                &[(21 * DAY, "merge/superseded", json!({"merged": "true"}))],
            ),
            item(
                "STALE",
                &[(
                    30 * DAY,
                    "verify/rung",
                    json!({"state": "pass", "rung": "1h"}),
                )],
            ),
            item(
                "HELD",
                &[(30 * DAY, "merge/gate", json!({"outcome": "human"}))],
            ),
            item("UNPLACED", &[(30 * DAY, "verify/checked", json!({}))]),
        ];
        let board = view(&def, &records, "");
        let shown_columns = board
            .items
            .iter()
            .filter_map(|doc| position_of(&def, &doc.body))
            .fold(BTreeMap::<String, usize>::new(), |mut columns, column| {
                *columns.entry(column).or_default() += 1;
                columns
            });
        let totals = board
            .windows
            .iter()
            .fold(shown_columns, |mut columns, (column, applied)| {
                if applied.hidden > 0 {
                    *columns.entry(column.clone()).or_default() += applied.hidden;
                }
                columns
            });
        assert_eq!(totals, count(&def, &records).columns);
        assert_eq!(board.items.len(), 4);
    }
}
