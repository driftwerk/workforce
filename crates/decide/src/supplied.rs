//! States the caller wrote: the `decide.ask` tool's `state` and a decision
//! step's `task`. Nothing is known about what in them decides the answer,
//! so only the collapse-and-budget rule applies.

use crate::text::{fit_value, StateBudget};
use serde_json::{json, Value};

/// A decision step's task as a state: JSON is the state as structure,
/// anything else is text.
pub fn parse(task: &str) -> Value {
    let trimmed = task.trim();
    Some(trimmed)
        .filter(|t| t.starts_with('{') || t.starts_with('['))
        .and_then(|t| serde_json::from_str(t).ok())
        .unwrap_or_else(|| Value::String(task.to_string()))
}

/// The state to send: text is wrapped, and every string is collapsed and
/// held to the budget. `None` when nothing was supplied — null, blank text
/// or an empty structure: there is nothing to decide about.
pub fn state(supplied: Value, budget: StateBudget) -> Option<Value> {
    let state = match supplied {
        Value::Null => return None,
        Value::String(text) if text.trim().is_empty() => return None,
        Value::Array(items) if items.is_empty() => return None,
        Value::Object(fields) if fields.is_empty() => return None,
        Value::String(text) => json!({ "state": text }),
        other => other,
    };
    Some(fit_value(&state, budget))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::ELISION;

    #[test]
    fn a_task_is_structure_when_it_is_json_and_text_otherwise() {
        assert_eq!(parse(r#" {"a": 1} "#), json!({"a": 1}));
        assert_eq!(parse("[1, 2]"), json!([1, 2]));
        assert_eq!(parse("plain words"), json!("plain words"));
        assert_eq!(parse("{not json"), json!("{not json"));
    }

    #[test]
    fn text_is_wrapped_and_a_short_state_is_sent_as_supplied() {
        let budget = StateBudget::DEFAULT;
        assert_eq!(
            state(json!("plain words"), budget),
            Some(json!({"state": "plain words"}))
        );
        let ticket = json!({"title": "Login fails", "votes": 3});
        assert_eq!(state(ticket.clone(), budget), Some(ticket));
        assert_eq!(
            state(json!({"votes": 3}), budget),
            Some(json!({"votes": 3}))
        );
    }

    #[test]
    fn nothing_supplied_is_no_state() {
        for nothing in [json!(null), json!(""), json!("  \n"), json!({}), json!([])] {
            assert_eq!(
                state(nothing.clone(), StateBudget::DEFAULT),
                None,
                "{nothing}"
            );
        }
        assert_eq!(state(parse("   "), StateBudget::DEFAULT), None);
    }

    #[test]
    fn a_long_state_is_collapsed_and_held_to_the_budget() {
        let body = format!("The export is empty. {}", "Further detail. ".repeat(400));
        let fitted = state(json!({"body": body, "id": 7}), StateBudget::chars(300)).unwrap();
        let kept = fitted["body"].as_str().unwrap();
        assert!(kept.chars().count() <= 300);
        assert!(kept.starts_with("The export is empty.") && kept.ends_with(ELISION));
        assert_eq!(fitted["id"], 7);
    }
}
