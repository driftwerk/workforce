//! One poll, as pure code: a tool's output against what the previous poll
//! kept gives the observations that fire and the state to keep for the next.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::condition::{text_of, CompiledCondition, Previous};
use super::definition::Source;
use super::event::{Observation, VALUE_KEY};

/// The largest watched part kept beside its hash, so `{{previous}}` and a
/// `changed` at a path can read it; a larger one keeps the hash alone.
pub const MAX_KEPT_VALUE_BYTES: usize = 16 * 1024;

/// What one key looked like at the last poll.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    /// Whether the condition held, so a level fires on its transition only.
    #[serde(default)]
    pub held: bool,
}

/// Every key the last poll saw.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ObservedState {
    #[serde(default)]
    pub entries: BTreeMap<String, Entry>,
}

/// What a poll found.
#[derive(Debug, Default)]
pub struct Outcome {
    pub fires: Vec<Observation>,
    pub state: ObservedState,
    /// Keys observed, fired or not.
    pub seen: usize,
    /// Items without a usable id.
    pub skipped: usize,
    /// Whether the output held more items than `max_items`.
    pub truncated: bool,
}

struct Candidate {
    key: String,
    item: Value,
    watched: Value,
    text: String,
}

/// The canonical hash of a watched part.
pub fn hash_of(value: &Value) -> String {
    blake3::hash(value.to_string().as_bytes())
        .to_hex()
        .to_string()
}

fn key_of(id: Option<&Value>) -> Option<String> {
    match id? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn candidate(source: &Source, key: String, item: &Value) -> Candidate {
    let watched = item.pointer(source.path()).cloned().unwrap_or(Value::Null);
    let text = match &source.text {
        Some(pointer) => text_of(item.pointer(pointer)).into_owned(),
        None => text_of(Some(&watched)).into_owned(),
    };
    Candidate {
        key,
        item: item.clone(),
        watched,
        text,
    }
}

fn candidates(
    source: &Source,
    output: &Value,
    max_items: usize,
) -> Result<(Vec<Candidate>, usize, bool), String> {
    let Some(pointer) = source.items.as_deref() else {
        return Ok((
            vec![candidate(source, VALUE_KEY.to_string(), output)],
            0,
            false,
        ));
    };
    let items = output
        .pointer(pointer)
        .and_then(Value::as_array)
        .ok_or_else(|| {
            format!("source.items '{pointer}' does not point at an array in the tool's output")
        })?;
    let truncated = items.len() > max_items;
    let mut skipped = 0;
    let found = items
        .iter()
        .take(max_items)
        .filter_map(|item| match key_of(item.pointer(source.item_id())) {
            Some(key) => Some(candidate(source, key, item)),
            None => {
                skipped += 1;
                None
            }
        })
        .collect();
    Ok((found, skipped, truncated))
}

fn kept(value: &Value) -> Option<Value> {
    (value.to_string().len() <= MAX_KEPT_VALUE_BYTES).then(|| value.clone())
}

/// Compare `output` with `previous` and decide what fires. A first poll of an
/// item source only records what is there; a first poll of a value evaluates
/// it. A level fires on the transition into holding, an event each time it
/// holds. A key that is no longer there is forgotten.
pub fn observe(
    source: &Source,
    condition: &CompiledCondition,
    output: &Value,
    previous: Option<&ObservedState>,
    now: DateTime<Utc>,
    max_items: usize,
) -> Result<Outcome, String> {
    let (found, skipped, truncated) = candidates(source, output, max_items)?;
    let baseline = previous.is_none() && source.watches_items();
    let mut outcome = Outcome {
        seen: found.len(),
        skipped,
        truncated,
        ..Outcome::default()
    };
    for Candidate {
        key,
        item,
        watched,
        text,
    } in found
    {
        let hash = hash_of(&watched);
        let prior = previous.and_then(|state| state.entries.get(&key));
        let holds = condition.holds(
            &watched,
            &hash,
            Previous {
                hash: prior.map(|e| e.hash.as_str()),
                value: prior.and_then(|e| e.value.as_ref()),
            },
        );
        let already_held = prior.is_some_and(|e| e.held);
        if holds && !baseline && (condition.is_event() || !already_held) {
            outcome.fires.push(Observation {
                source: source.tool.clone(),
                key: key.clone(),
                observed_at: now,
                text,
                value: watched.clone(),
                previous: prior.and_then(|e| e.value.clone()),
                item,
            });
        }
        outcome.state.entries.insert(
            key,
            Entry {
                hash,
                value: kept(&watched),
                held: holds,
            },
        );
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::monitor::definition::Condition;

    fn source(toml: &str) -> Source {
        toml::from_str(toml).expect("source toml")
    }

    fn items_source() -> Source {
        source("tool = \"jira.search\"\nitems = \"/issues\"\nitem_id = \"/key\"\npath = \"/status\"\ntext = \"/summary\"")
    }

    fn compiled(toml: &str) -> CompiledCondition {
        let condition: Condition = toml::from_str(toml).expect("condition toml");
        CompiledCondition::compile(&condition).expect("compiles")
    }

    fn issues(rows: &[(&str, &str)]) -> Value {
        json!({
            "issues": rows
                .iter()
                .map(|(key, status)| json!({ "key": key, "status": status, "summary": format!("{key}: {status}") }))
                .collect::<Vec<_>>()
        })
    }

    fn keys(fires: &[Observation]) -> Vec<&str> {
        fires.iter().map(|o| o.key.as_str()).collect()
    }

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    #[test]
    fn a_first_poll_of_items_baselines_and_later_polls_fire_new_items_once() {
        let src = items_source();
        let always = compiled("type = \"always\"");
        let first = observe(&src, &always, &issues(&[("A-1", "open")]), None, now(), 100).unwrap();
        assert!(first.fires.is_empty(), "baseline fires nothing");
        assert_eq!(first.seen, 1);
        assert!(first.state.entries.contains_key("A-1"));

        let second = observe(
            &src,
            &always,
            &issues(&[("A-1", "open"), ("A-2", "open")]),
            Some(&first.state),
            now(),
            100,
        )
        .unwrap();
        assert_eq!(keys(&second.fires), ["A-2"]);
        assert_eq!(second.fires[0].text, "A-2: open");
        assert_eq!(second.fires[0].value, json!("open"));
        assert_eq!(second.fires[0].source, "jira.search");
        assert!(second.fires[0].previous.is_none());

        let third = observe(
            &src,
            &always,
            &issues(&[("A-1", "open"), ("A-2", "open")]),
            Some(&second.state),
            now(),
            100,
        )
        .unwrap();
        assert!(third.fires.is_empty(), "an item fires once");
    }

    #[test]
    fn a_level_fires_on_the_transition_and_stays_quiet_while_it_holds() {
        let src = items_source();
        let done = compiled("type = \"equals\"\nvalue = \"done\"");
        let state = observe(
            &src,
            &done,
            &issues(&[("A-1", "open"), ("A-2", "done")]),
            None,
            now(),
            100,
        )
        .unwrap()
        .state;
        assert!(
            state.entries["A-2"].held,
            "an item already holding is baselined as held"
        );

        let moved = observe(
            &src,
            &done,
            &issues(&[("A-1", "done"), ("A-2", "done")]),
            Some(&state),
            now(),
            100,
        )
        .unwrap();
        assert_eq!(keys(&moved.fires), ["A-1"], "only the transition fires");
        assert_eq!(moved.fires[0].previous, Some(json!("open")));

        let held = observe(
            &src,
            &done,
            &issues(&[("A-1", "done"), ("A-2", "done")]),
            Some(&moved.state),
            now(),
            100,
        )
        .unwrap();
        assert!(held.fires.is_empty());

        let back = observe(
            &src,
            &done,
            &issues(&[("A-1", "open"), ("A-2", "done")]),
            Some(&held.state),
            now(),
            100,
        )
        .unwrap();
        assert!(back.fires.is_empty(), "leaving the level is silent");
        let again = observe(
            &src,
            &done,
            &issues(&[("A-1", "done"), ("A-2", "done")]),
            Some(&back.state),
            now(),
            100,
        )
        .unwrap();
        assert_eq!(
            keys(&again.fires),
            ["A-1"],
            "and re-entering it fires again"
        );

        let fresh = observe(
            &src,
            &done,
            &issues(&[("A-3", "done")]),
            Some(&again.state),
            now(),
            100,
        )
        .unwrap();
        assert_eq!(keys(&fresh.fires), ["A-3"], "a new item that holds fires");
        assert_eq!(
            fresh.state.entries.len(),
            1,
            "keys no longer there are forgotten"
        );
        let returned = observe(
            &src,
            &done,
            &issues(&[("A-1", "done"), ("A-3", "done")]),
            Some(&fresh.state),
            now(),
            100,
        )
        .unwrap();
        assert_eq!(
            keys(&returned.fires),
            ["A-1"],
            "and fire again when they return"
        );
    }

    #[test]
    fn an_event_fires_on_every_change_and_not_for_a_new_item() {
        let src = items_source();
        let changed = compiled("type = \"changed\"");
        let state = observe(
            &src,
            &changed,
            &issues(&[("A-1", "open")]),
            None,
            now(),
            100,
        )
        .unwrap()
        .state;
        let same = observe(
            &src,
            &changed,
            &issues(&[("A-1", "open"), ("A-2", "open")]),
            Some(&state),
            now(),
            100,
        )
        .unwrap();
        assert!(same.fires.is_empty(), "a new item has not changed");
        let moved = observe(
            &src,
            &changed,
            &issues(&[("A-1", "done"), ("A-2", "open")]),
            Some(&same.state),
            now(),
            100,
        )
        .unwrap();
        assert_eq!(keys(&moved.fires), ["A-1"]);
        let moved_back = observe(
            &src,
            &changed,
            &issues(&[("A-1", "open"), ("A-2", "open")]),
            Some(&moved.state),
            now(),
            100,
        )
        .unwrap();
        assert_eq!(
            keys(&moved_back.fires),
            ["A-1"],
            "each change is a new fact"
        );
    }

    #[test]
    fn a_value_is_evaluated_on_its_first_poll_and_changed_waits_for_a_second() {
        let src = source("tool = \"http.request\"\npath = \"/body\"");
        let page = |body: &str| json!({ "status": 200, "body": body });
        let contains = compiled("type = \"contains\"\nvalues = [\"sold out\"]");
        let first = observe(&src, &contains, &page("SOLD OUT"), None, now(), 100).unwrap();
        assert_eq!(
            keys(&first.fires),
            [VALUE_KEY],
            "a level that already holds fires at once"
        );
        assert_eq!(first.fires[0].item, page("SOLD OUT"));
        let held = observe(
            &src,
            &contains,
            &page("sold out"),
            Some(&first.state),
            now(),
            100,
        )
        .unwrap();
        assert!(held.fires.is_empty());

        let changed = compiled("type = \"changed\"");
        let first = observe(&src, &changed, &page("a"), None, now(), 100).unwrap();
        assert!(first.fires.is_empty());
        let same = observe(&src, &changed, &page("a"), Some(&first.state), now(), 100).unwrap();
        assert!(same.fires.is_empty());
        let moved = observe(&src, &changed, &page("b"), Some(&same.state), now(), 100).unwrap();
        assert_eq!(keys(&moved.fires), [VALUE_KEY]);
        assert_eq!(moved.fires[0].previous, Some(json!("a")));
        assert_eq!(moved.fires[0].text, "b");
    }

    #[test]
    fn items_need_an_array_and_a_usable_id_and_are_capped() {
        let src = items_source();
        let always = compiled("type = \"always\"");
        let err = observe(
            &src,
            &always,
            &json!({ "issues": "nope" }),
            None,
            now(),
            100,
        )
        .unwrap_err();
        assert!(err.contains("does not point at an array"), "{err}");

        let output = json!({ "issues": [
            { "key": "A-1", "status": "open" },
            { "status": "no key" },
            { "key": 7, "status": "numeric key" },
            { "key": "", "status": "empty key" },
            { "key": "A-9", "status": "beyond the cap" },
        ]});
        let outcome = observe(
            &src,
            &always,
            &output,
            Some(&ObservedState::default()),
            now(),
            4,
        )
        .unwrap();
        assert_eq!(keys(&outcome.fires), ["A-1", "7"]);
        assert_eq!((outcome.seen, outcome.skipped), (2, 2));
        assert!(outcome.truncated);
        assert_eq!(
            outcome.fires[0].text, "",
            "a text pointer that resolves to nothing renders empty"
        );
    }

    #[test]
    fn a_large_part_keeps_its_hash_but_not_its_copy() {
        let src = source("tool = \"http.request\"");
        let changed = compiled("type = \"changed\"");
        let big = json!({ "body": "x".repeat(MAX_KEPT_VALUE_BYTES + 1) });
        let first = observe(&src, &changed, &big, None, now(), 100).unwrap();
        let entry = &first.state.entries[VALUE_KEY];
        assert!(entry.value.is_none());
        assert_eq!(entry.hash, hash_of(&big));
        let moved = observe(
            &src,
            &changed,
            &json!({ "body": "small" }),
            Some(&first.state),
            now(),
            100,
        )
        .unwrap();
        assert_eq!(keys(&moved.fires), [VALUE_KEY]);
        assert!(
            moved.fires[0].previous.is_none(),
            "nothing kept to render as previous"
        );
        assert!(moved.state.entries[VALUE_KEY].value.is_some());
    }

    #[test]
    fn state_round_trips_as_json() {
        let state = ObservedState {
            entries: BTreeMap::from([(
                "k".to_string(),
                Entry {
                    hash: "h".into(),
                    value: None,
                    held: true,
                },
            )]),
        };
        let json = serde_json::to_value(&state).unwrap();
        assert!(json["entries"]["k"].get("value").is_none());
        assert_eq!(
            serde_json::from_value::<ObservedState>(json).unwrap(),
            state
        );
        assert_eq!(
            serde_json::from_value::<ObservedState>(json!({})).unwrap(),
            ObservedState::default()
        );
    }
}
