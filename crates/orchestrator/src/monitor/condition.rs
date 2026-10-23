//! A source monitor's condition compiled for evaluation: every regex built
//! once at load, every comparand normalised, so the poll path only tests.

use std::borrow::Cow;

use eyre::{Result, WrapErr};
use regex::Regex;
use serde_json::Value;

use super::definition::{Condition, ConditionKind};

/// What the previous poll kept of the same key, for `changed`: the hash is
/// always there, the value only when it was small enough to keep.
#[derive(Clone, Copy, Default)]
pub struct Previous<'a> {
    pub hash: Option<&'a str>,
    pub value: Option<&'a Value>,
}

#[derive(Clone, Copy)]
enum Compare {
    Gt,
    Lt,
    Gte,
    Lte,
}

impl Compare {
    fn holds(self, got: f64, expected: f64) -> bool {
        match self {
            Self::Gt => got > expected,
            Self::Lt => got < expected,
            Self::Gte => got >= expected,
            Self::Lte => got <= expected,
        }
    }
}

enum Test {
    Always,
    Changed,
    Equals(Value),
    Contains(Vec<String>),
    Regex(Regex),
    Compare(Compare, f64),
    Exists,
    All(Vec<CompiledCondition>),
    Any(Vec<CompiledCondition>),
}

pub struct CompiledCondition {
    kind: ConditionKind,
    test: Test,
    path: String,
    negate: bool,
    event: bool,
}

/// A number as JSON carries it, or as a numeric string.
pub fn as_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// A string as itself, anything else as its compact JSON, nothing as "".
pub fn text_of(value: Option<&Value>) -> Cow<'_, str> {
    match value {
        None => Cow::Borrowed(""),
        Some(Value::String(s)) => Cow::Borrowed(s),
        Some(other) => Cow::Owned(other.to_string()),
    }
}

impl CompiledCondition {
    pub fn compile(condition: &Condition) -> Result<Self> {
        let parts = || {
            condition
                .of
                .iter()
                .map(Self::compile)
                .collect::<Result<Vec<_>>>()
        };
        let number = || {
            condition.value.as_ref().and_then(as_number).ok_or_else(|| {
                eyre::eyre!("{} condition needs a numeric value", condition.kind.name())
            })
        };
        let test = match condition.kind {
            ConditionKind::Always => Test::Always,
            ConditionKind::Changed => Test::Changed,
            ConditionKind::Equals => Test::Equals(
                condition
                    .value
                    .clone()
                    .ok_or_else(|| eyre::eyre!("equals condition needs a value"))?,
            ),
            ConditionKind::Contains => Test::Contains(
                condition
                    .values
                    .iter()
                    .filter(|v| !v.is_empty())
                    .map(|v| v.to_lowercase())
                    .collect(),
            ),
            ConditionKind::Regex => {
                let pattern = condition
                    .pattern
                    .as_deref()
                    .ok_or_else(|| eyre::eyre!("regex condition needs a pattern"))?;
                Test::Regex(
                    Regex::new(pattern)
                        .wrap_err_with(|| format!("invalid condition regex '{pattern}'"))?,
                )
            }
            ConditionKind::Gt => Test::Compare(Compare::Gt, number()?),
            ConditionKind::Lt => Test::Compare(Compare::Lt, number()?),
            ConditionKind::Gte => Test::Compare(Compare::Gte, number()?),
            ConditionKind::Lte => Test::Compare(Compare::Lte, number()?),
            ConditionKind::Exists => Test::Exists,
            ConditionKind::All => Test::All(parts()?),
            ConditionKind::Any => Test::Any(parts()?),
        };
        Ok(Self {
            kind: condition.kind,
            test,
            path: condition.path.clone().unwrap_or_default(),
            negate: condition.negate,
            event: condition.is_event(),
        })
    }

    pub fn kind(&self) -> ConditionKind {
        self.kind
    }

    /// Whether this condition fires each time it holds (it contains
    /// `changed`) rather than on the transition into holding.
    pub fn is_event(&self) -> bool {
        self.event
    }

    /// Whether the condition holds for `current`, whose canonical hash is
    /// `current_hash`, given what the previous poll kept of the same key.
    pub fn holds(&self, current: &Value, current_hash: &str, previous: Previous<'_>) -> bool {
        let got = current.pointer(&self.path);
        let held = match &self.test {
            Test::Always => true,
            Test::Changed => match previous.value {
                Some(prev) if !self.path.is_empty() => prev.pointer(&self.path) != got,
                _ => previous.hash.is_some_and(|hash| hash != current_hash),
            },
            Test::Equals(expected) => match (as_number(expected), got.and_then(as_number)) {
                (Some(want), Some(have)) => want == have,
                _ => got == Some(expected),
            },
            Test::Contains(needles) => {
                let text = text_of(got).to_lowercase();
                needles.iter().any(|needle| text.contains(needle))
            }
            Test::Regex(re) => re.is_match(&text_of(got)),
            Test::Compare(compare, expected) => got
                .and_then(as_number)
                .is_some_and(|have| compare.holds(have, *expected)),
            Test::Exists => got.is_some_and(|v| !v.is_null()),
            Test::All(parts) => parts
                .iter()
                .all(|part| part.holds(current, current_hash, previous)),
            Test::Any(parts) => parts
                .iter()
                .any(|part| part.holds(current, current_hash, previous)),
        };
        held != self.negate
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn condition(toml: &str) -> CompiledCondition {
        let parsed: Condition = toml::from_str(toml).expect("condition toml");
        CompiledCondition::compile(&parsed).expect("compiles")
    }

    fn holds(cond: &CompiledCondition, current: &Value) -> bool {
        cond.holds(current, "h1", Previous::default())
    }

    #[test]
    fn leaves_test_the_watched_part_at_their_path() {
        let status = json!({ "status": "completed", "runs": 3, "pct": "97.5", "tags": ["a", "b"] });
        assert!(holds(&condition("type = \"always\""), &status));
        assert!(holds(
            &condition("type = \"equals\"\npath = \"/status\"\nvalue = \"completed\""),
            &status
        ));
        assert!(!holds(
            &condition("type = \"equals\"\npath = \"/status\"\nvalue = \"queued\""),
            &status
        ));
        assert!(
            holds(
                &condition("type = \"equals\"\npath = \"/runs\"\nvalue = 3.0"),
                &status
            ),
            "numbers compare numerically"
        );
        assert!(
            holds(
                &condition("type = \"equals\"\npath = \"/pct\"\nvalue = 97.5"),
                &status
            ),
            "a numeric string equals a number"
        );
        assert!(holds(
            &condition("type = \"contains\"\npath = \"/status\"\nvalues = [\"COMPLETE\"]"),
            &status
        ));
        assert!(
            holds(
                &condition("type = \"contains\"\npath = \"/tags\"\nvalues = [\"\\\"b\\\"\"]"),
                &status
            ),
            "a non-string is searched as its JSON text"
        );
        assert!(holds(
            &condition("type = \"regex\"\npath = \"/status\"\npattern = \"^comp\""),
            &status
        ));
        assert!(holds(
            &condition("type = \"gt\"\npath = \"/runs\"\nvalue = 2"),
            &status
        ));
        assert!(!holds(
            &condition("type = \"gt\"\npath = \"/runs\"\nvalue = 3"),
            &status
        ));
        assert!(holds(
            &condition("type = \"gte\"\npath = \"/runs\"\nvalue = 3"),
            &status
        ));
        assert!(holds(
            &condition("type = \"lt\"\npath = \"/pct\"\nvalue = 100"),
            &status
        ));
        assert!(holds(
            &condition("type = \"lte\"\npath = \"/pct\"\nvalue = 97.5"),
            &status
        ));
        assert!(
            !holds(
                &condition("type = \"gt\"\npath = \"/status\"\nvalue = 1"),
                &status
            ),
            "a non-number never compares"
        );
        assert!(holds(
            &condition("type = \"exists\"\npath = \"/status\""),
            &status
        ));
        assert!(!holds(
            &condition("type = \"exists\"\npath = \"/missing\""),
            &status
        ));
        assert!(!holds(
            &condition("type = \"exists\"\npath = \"/none\""),
            &json!({ "none": null })
        ));
    }

    #[test]
    fn an_empty_path_is_the_whole_watched_part() {
        assert!(holds(
            &condition("type = \"equals\"\nvalue = \"ok\""),
            &json!("ok")
        ));
        assert!(holds(
            &condition("type = \"contains\"\nvalues = [\"err\"]"),
            &json!("Error: x")
        ));
        assert!(holds(&condition("type = \"gt\"\nvalue = 10"), &json!(11)));
    }

    #[test]
    fn negate_inverts_and_combinators_nest() {
        let status = json!({ "status": "failed", "runs": 3 });
        assert!(holds(
            &condition("type = \"equals\"\npath = \"/status\"\nvalue = \"ok\"\nnegate = true"),
            &status
        ));
        let all = condition(
            "type = \"all\"\nof = [{ type = \"equals\", path = \"/status\", value = \"failed\" }, { type = \"gte\", path = \"/runs\", value = 3 }]",
        );
        assert!(holds(&all, &status));
        assert!(!holds(&all, &json!({ "status": "failed", "runs": 1 })));
        let any = condition(
            "type = \"any\"\nnegate = true\nof = [{ type = \"exists\", path = \"/nope\" }, { type = \"any\", of = [{ type = \"lt\", path = \"/runs\", value = 0 }] }]",
        );
        assert!(holds(&any, &status), "negated any of nothing holding");
    }

    #[test]
    fn changed_compares_with_the_previous_value_or_its_hash() {
        let whole = condition("type = \"changed\"");
        let current = json!({ "status": "ok", "ts": 2 });
        assert!(
            !whole.holds(&current, "h2", Previous::default()),
            "nothing to compare"
        );
        assert!(whole.holds(
            &current,
            "h2",
            Previous {
                hash: Some("h1"),
                value: None
            }
        ));
        assert!(!whole.holds(
            &current,
            "h2",
            Previous {
                hash: Some("h2"),
                value: None
            }
        ));

        let status = condition("type = \"changed\"\npath = \"/status\"");
        let previous = json!({ "status": "ok", "ts": 1 });
        assert!(
            !status.holds(
                &current,
                "h2",
                Previous {
                    hash: Some("h1"),
                    value: Some(&previous)
                }
            ),
            "only the timestamp moved"
        );
        assert!(status.holds(
            &json!({ "status": "failed", "ts": 2 }),
            "h3",
            Previous {
                hash: Some("h1"),
                value: Some(&previous)
            }
        ));
        assert!(
            status.holds(
                &current,
                "h2",
                Previous {
                    hash: Some("h1"),
                    value: None
                }
            ),
            "with no kept value, any change of the whole part counts"
        );
        assert!(status.is_event());
        assert!(!whole.negate);
        assert_eq!(whole.kind(), ConditionKind::Changed);
    }

    #[test]
    fn text_of_renders_strings_bare_and_the_rest_as_json() {
        assert_eq!(text_of(None), "");
        assert_eq!(text_of(Some(&json!("plain"))), "plain");
        assert_eq!(text_of(Some(&json!({ "a": 1 }))), "{\"a\":1}");
    }
}
