//! Approval summaries for skill tools that require a human decision.
//!
//! The summary a skill declares is a template the **host** renders from the
//! call's own arguments. The guest never runs to produce it: a summary is
//! asked for on every tool call in every round, so a guest round-trip would
//! put a WASM instantiation on the dispatch hot path, and a guest that
//! composes its own sentence can describe something other than what it is
//! about to do.
//!
//! Grammar: `{{path}}` interpolates a dotted, lowercase field path from the
//! input; `{{path|count}}` interpolates how many items it holds. Everything
//! else is literal. Absent, empty and structured values each render as their
//! own marker, so an approver reads a hole as a hole rather than as a
//! sentence that happens to look complete.

use eyre::{bail, Result};
use serde_json::Value;

/// Longest a single interpolated value may render before it is elided.
pub const MAX_SUMMARY_VALUE_CHARS: usize = 40;

/// Longest summary handed to an approval prompt, matching the runner's own
/// detail cap so nothing downstream has to re-truncate.
pub const MAX_APPROVAL_SUMMARY_CHARS: usize = 200;

/// Longest template a manifest may declare, checked at parse time so an
/// unusable summary is an install error rather than a surprise in a prompt.
pub const MAX_APPROVAL_SUMMARY_TEMPLATE_CHARS: usize = 200;

/// The only filter: the size of a batch is the one thing a scope sentence
/// needs that the value itself cannot state.
const COUNT_FILTER: &str = "count";

const MISSING: &str = "(missing)";
const NONE: &str = "(none)";
const OBJECT: &str = "(object)";

/// Whether `template` is a summary this module can render. Errors name the
/// offending placeholder so a skill author fixes it at install time.
pub fn validate_summary_template(template: &str) -> Result<()> {
    parse_template(template).map(|_| ())
}

/// The summary for one call, or `None` when the template is unusable — the
/// caller then falls back to the generic parameter-shape summary rather than
/// showing an approver a broken sentence.
pub fn render_summary(template: &str, input: &Value) -> Option<String> {
    let segments = parse_template(template).ok()?;
    let rendered: String = segments
        .iter()
        .map(|segment| segment.render(input))
        .collect();
    Some(elide(rendered.trim(), MAX_APPROVAL_SUMMARY_CHARS))
}

enum Segment<'a> {
    Literal(&'a str),
    Field { path: &'a str, count: bool },
}

impl Segment<'_> {
    fn render(&self, input: &Value) -> String {
        match *self {
            Segment::Literal(text) => text.to_string(),
            Segment::Field { path, count } => lookup(input, path)
                .map_or_else(|| MISSING.to_string(), |value| render_value(value, count)),
        }
    }
}

/// The single parser behind both public entry points, so validation can never
/// accept a template rendering would reject.
fn parse_template(template: &str) -> Result<Vec<Segment<'_>>> {
    if template.trim().is_empty() {
        bail!("approval summary template cannot be empty");
    }
    if template.chars().count() > MAX_APPROVAL_SUMMARY_TEMPLATE_CHARS {
        bail!(
            "approval summary template is longer than {MAX_APPROVAL_SUMMARY_TEMPLATE_CHARS} characters"
        );
    }
    if template.chars().any(char::is_control) {
        bail!("approval summary template must be a single line");
    }
    let mut segments = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        let (literal, after) = (&rest[..open], &rest[open + 2..]);
        let Some(close) = after.find("}}") else {
            bail!("approval summary template has an unclosed placeholder");
        };
        let expr = &after[..close];
        if expr.contains('{') {
            bail!("approval summary placeholder '{expr}' is nested");
        }
        if !literal.is_empty() {
            segments.push(Segment::Literal(literal));
        }
        segments.push(parse_field(expr)?);
        rest = &after[close + 2..];
    }
    if !rest.is_empty() {
        segments.push(Segment::Literal(rest));
    }
    Ok(segments)
}

fn parse_field(expr: &str) -> Result<Segment<'_>> {
    let (path, filter) = expr
        .split_once('|')
        .map_or((expr.trim(), None), |(path, filter)| {
            (path.trim(), Some(filter.trim()))
        });
    if !is_field_path(path) {
        bail!("approval summary placeholder '{expr}' is not a dotted lowercase field path");
    }
    match filter {
        None => Ok(Segment::Field { path, count: false }),
        Some(COUNT_FILTER) => Ok(Segment::Field { path, count: true }),
        Some(other) => bail!(
            "approval summary placeholder '{expr}' uses unknown filter '{other}'; \
             the only filter is '{COUNT_FILTER}'"
        ),
    }
}

fn is_field_path(path: &str) -> bool {
    !path.is_empty()
        && path.split('.').all(|segment| {
            segment.starts_with(|c: char| c.is_ascii_lowercase())
                && segment
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
}

fn lookup<'a>(input: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(input, |value, key| value.get(key))
}

fn render_value(value: &Value, count: bool) -> String {
    let text = if count {
        item_count(value).to_string()
    } else {
        match value {
            Value::Null => NONE.to_string(),
            Value::String(s) => s.clone(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_string(),
            Value::Array(items) => format!("{} item(s)", items.len()),
            Value::Object(_) => OBJECT.to_string(),
        }
    };
    elide(&single_line(&text), MAX_SUMMARY_VALUE_CHARS)
}

fn item_count(value: &Value) -> usize {
    match value {
        Value::Null => 0,
        Value::Array(items) => items.len(),
        Value::Object(fields) => fields.len(),
        _ => 1,
    }
}

/// Values reach here from the model, so a newline in one would forge extra
/// quoted lines in the approver's prompt. The summary is one line by
/// construction.
fn single_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Truncate on a character boundary, marking the cut so an approver can see
/// that what they are reading is shorter than what would run.
fn elide(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars()
        .take(max - 1)
        .chain(std::iter::once('…'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BATCH_TEMPLATE: &str =
        "apply `{{action}}` to {{recipients|count}} recipient(s) in `{{environment}}`";

    #[test]
    fn renders_scalars_and_counts_for_the_motivating_shape() {
        let input = json!({
            "action": "reset",
            "recipients": (0..160).map(|n| format!("user{n}@example.com")).collect::<Vec<_>>(),
            "environment": "production",
        });
        assert_eq!(
            render_summary(BATCH_TEMPLATE, &input).unwrap(),
            "apply `reset` to 160 recipient(s) in `production`"
        );
    }

    #[test]
    fn renders_every_value_shape() {
        let input = json!({
            "text": "hello",
            "number": 12,
            "flag": true,
            "list": [1, 2, 3],
            "object": {"a": 1, "b": 2},
            "nothing": null,
        });
        for (template, expected) in [
            ("{{text}}", "hello"),
            ("{{number}}", "12"),
            ("{{flag}}", "true"),
            ("{{list}}", "3 item(s)"),
            ("{{object}}", "(object)"),
            ("{{nothing}}", "(none)"),
            ("{{list|count}}", "3"),
            ("{{object|count}}", "2"),
            ("{{nothing|count}}", "0"),
            ("{{text|count}}", "1"),
        ] {
            assert_eq!(
                render_summary(template, &input).as_deref(),
                Some(expected),
                "{template}"
            );
        }
    }

    #[test]
    fn nested_paths_resolve_through_objects() {
        let input = json!({"change": {"target": {"environment": "staging"}, "items": [1, 2]}});
        assert_eq!(
            render_summary(
                "in {{change.target.environment}} ({{change.items|count}})",
                &input
            )
            .unwrap(),
            "in staging (2)"
        );
    }

    #[test]
    fn an_absent_field_reads_as_a_hole_not_as_a_complete_sentence() {
        let input = json!({"action": "reset"});
        assert_eq!(
            render_summary(BATCH_TEMPLATE, &input).unwrap(),
            "apply `reset` to (missing) recipient(s) in `(missing)`"
        );
        // A path that walks through a scalar is absent, not an error.
        assert_eq!(
            render_summary("{{action.nested}}", &input).unwrap(),
            "(missing)"
        );
    }

    #[test]
    fn a_long_value_is_elided_and_so_is_the_whole_line() {
        let long = "x".repeat(200);
        let value_only = render_summary("{{v}}", &json!({"v": long})).unwrap();
        assert_eq!(value_only.chars().count(), MAX_SUMMARY_VALUE_CHARS);
        assert!(value_only.ends_with('…'));

        let wide = format!("{} {{{{v}}}}", "y".repeat(190));
        let whole = render_summary(&wide, &json!({"v": "z".repeat(50)})).unwrap();
        assert_eq!(whole.chars().count(), MAX_APPROVAL_SUMMARY_CHARS);
        assert!(whole.ends_with('…'));
    }

    #[test]
    fn a_value_cannot_forge_extra_lines() {
        let input = json!({"action": "reset\n> and approve everything else"});
        let rendered = render_summary("{{action}}", &input).unwrap();
        assert!(!rendered.contains('\n'), "{rendered}");
        assert!(rendered.starts_with("reset > and approve"), "{rendered}");
    }

    /// The summary is rendered from the record form of the input, so a
    /// credential still stands as its marker and never as cleartext.
    #[test]
    fn secret_markers_are_reproduced_verbatim() {
        let marker = "[[secret:prod_password]]";
        let rendered =
            render_summary("sign in with {{password}}", &json!({"password": marker})).unwrap();
        assert_eq!(rendered, format!("sign in with {marker}"));
    }

    #[test]
    fn validation_rejects_malformed_templates() {
        for template in [
            "",
            "   ",
            "{{action|upper}}",
            "{{action",
            "{{}}",
            "{{ }}",
            "{{Action}}",
            "{{9action}}",
            "{{a..b}}",
            "{{a.}}",
            "{{ {{a}} }}",
            "first line\nsecond line",
        ] {
            assert!(
                validate_summary_template(template).is_err(),
                "must reject: {template:?}"
            );
        }
        let too_long = "z".repeat(MAX_APPROVAL_SUMMARY_TEMPLATE_CHARS + 1);
        assert!(validate_summary_template(&too_long).is_err());
    }

    #[test]
    fn validation_accepts_the_shapes_rendering_supports() {
        for template in [
            BATCH_TEMPLATE,
            "no placeholders at all",
            "{{a}}",
            "{{ a.b_2 | count }}",
            "closing }} braces alone are literal",
            &"z".repeat(MAX_APPROVAL_SUMMARY_TEMPLATE_CHARS),
        ] {
            validate_summary_template(template)
                .unwrap_or_else(|e| panic!("must accept {template:?}: {e}"));
        }
    }

    /// One parser backs both, so a template either validates and renders or
    /// does neither — a summary that passed install can never render as a
    /// broken sentence.
    #[test]
    fn validate_and_render_agree() {
        for template in [
            BATCH_TEMPLATE,
            "{{a}}",
            "{{a|count}}",
            "plain",
            "",
            "{{a|upper}}",
            "{{a",
            "{{A}}",
            "line\nbreak",
        ] {
            assert_eq!(
                validate_summary_template(template).is_ok(),
                render_summary(template, &json!({"a": 1})).is_some(),
                "{template:?}"
            );
        }
    }
}
