//! Engine-rendered step reports. A `report` with `report_render = "template"`
//! is a Jinja template rendered against the instance variables once the
//! step's outputs exist, and posted verbatim — no model writes it.

use std::collections::HashSet;

use minijinja::{AutoEscape, Environment, ErrorKind, UndefinedBehavior};

/// Instruction budget per render, so an authored template cannot wedge the
/// runtime.
const RENDER_FUEL: u64 = 1_000_000;

/// Printing or iterating a missing value fails the step; testing one in
/// `{% if %}` is allowed so a skipped step's output can gate a section. Block
/// tags on their own lines leave no blank lines behind. `debug()` is removed:
/// it renders the whole context, which is how a secret marker or every prior
/// step's output would reach the channel past the reference checks.
fn environment() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::SemiStrict);
    env.set_auto_escape_callback(|_| AutoEscape::None);
    env.set_trim_blocks(true);
    env.set_lstrip_blocks(true);
    env.set_fuel(Some(RENDER_FUEL));
    env.remove_global("debug");
    env
}

/// Compile-only check; the error names the offending line.
pub fn check(template: &str) -> Result<(), String> {
    environment()
        .template_from_str(template)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Dotted variable paths the template reads from the instance variables. Loop
/// variables, filters and the environment's globals are not references. Empty
/// when the template does not compile — [`check`] owns that error.
pub fn references(template: &str) -> HashSet<String> {
    let env = environment();
    let globals: HashSet<&str> = env.globals().map(|(name, _)| name).collect();
    env.template_from_str(template)
        .map(|compiled| {
            compiled
                .undeclared_variables(true)
                .into_iter()
                .filter(|path| !globals.contains(root(path)))
                .collect()
        })
        .unwrap_or_default()
}

/// Render `template` against `variables`, trimmed.
pub fn render(template: &str, variables: &serde_json::Value) -> Result<String, String> {
    render_in(environment(), template, variables)
}

/// Render as [`render`] does, but a missing value prints as empty instead of
/// failing: for a failure report, which may run before the steps it names.
pub fn render_lenient(template: &str, variables: &serde_json::Value) -> Result<String, String> {
    let mut env = environment();
    env.set_undefined_behavior(UndefinedBehavior::Chainable);
    render_in(env, template, variables)
}

fn render_in(
    env: Environment<'static>,
    template: &str,
    variables: &serde_json::Value,
) -> Result<String, String> {
    let context = structured(variables);
    env.template_from_named_str("report", template)
        .and_then(|compiled| compiled.render(&context))
        .map(|text| text.trim().to_string())
        .map_err(|e| describe(&e, template, &context))
}

/// minijinja reports a missing value as just "undefined value (in report:N)",
/// so an undefined error is completed with the source line and every
/// reference the context cannot resolve. The plain `Display` form is used
/// throughout: the alternate form appends every variable the template had
/// looked up, which for a report is every prior step's outputs.
fn describe(error: &minijinja::Error, template: &str, context: &serde_json::Value) -> String {
    if error.kind() != ErrorKind::UndefinedError {
        return error.to_string();
    }
    let line = error
        .line()
        .and_then(|n| template.lines().nth(n.checked_sub(1)?))
        .map(str::trim)
        .unwrap_or_default();
    let mut unresolved: Vec<String> = references(template)
        .into_iter()
        .filter(|path| resolve(context, path).is_none())
        .collect();
    unresolved.sort();
    format!(
        "{error}; line: `{line}`; unresolved: {}",
        unresolved.join(", ")
    )
}

fn resolve<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    path.split('.')
        .try_fold(value, |current, segment| current.get(segment))
}

pub(super) fn root(path: &str) -> &str {
    path.split('.').next().unwrap_or(path)
}

/// Step outputs travel as text. A template is the one consumer that needs
/// data, so every string leaf that is a JSON array, a JSON object, or a
/// boolean word (`true`/`false`, any case — models write both) is exposed as
/// that value. Numbers and prose stay text — `"3.10"` must not become `3.1`
/// in a report.
fn structured(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), structured(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(structured).collect()),
        Value::String(text) => parse_structured(text).unwrap_or_else(|| value.clone()),
        other => other.clone(),
    }
}

fn parse_structured(text: &str) -> Option<serde_json::Value> {
    match text.trim() {
        word if word.eq_ignore_ascii_case("true") => Some(serde_json::Value::Bool(true)),
        word if word.eq_ignore_ascii_case("false") => Some(serde_json::Value::Bool(false)),
        json if json.starts_with('[') || json.starts_with('{') => serde_json::from_str(json).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_lenient_render_prints_missing_values_as_empty() {
        let vars = json!({"steps": {"pick": {"ticket_id": "ENG-1"}}});
        let template = "{{ steps.pick.ticket_id }} → {{ steps.pr.pr_url }}";
        assert!(render(template, &vars).is_err());
        assert_eq!(render_lenient(template, &vars).unwrap(), "ENG-1 →");
    }

    #[test]
    fn renders_json_string_arrays_and_bool_strings_as_data() {
        let vars = json!({"steps": {"fetch": {
            "rate_limited": "true",
            "items": "[{\"key\": \"ENG-1\"}, {\"key\": \"ENG-2\"}]"
        }}});
        let text = render(
            "{% for i in steps.fetch.items %}{{ i.key }} {% endfor %}\
             {% if steps.fetch.rate_limited %}partial{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(text, "ENG-1 ENG-2 partial");
    }

    #[test]
    fn for_else_covers_an_empty_json_array() {
        let vars = json!({"steps": {"t": {"items": "[]"}}});
        let text = render(
            "{% for i in steps.t.items %}x{% else %}none{% endfor %}",
            &vars,
        )
        .unwrap();
        assert_eq!(text, "none");
    }

    #[test]
    fn block_tags_on_their_own_lines_leave_no_blank_lines() {
        let text = render("A\n{% if true %}\nB\n{% endif %}\nC", &json!({})).unwrap();
        assert_eq!(text, "A\nB\nC");
    }

    #[test]
    fn scalars_that_are_not_containers_or_booleans_stay_text() {
        let vars = json!({"v": "3.10", "n": "007", "s": "hello", "z": "null", "b": "[broken"});
        let text = render("{{ v }} {{ n }} {{ s }} {{ z }} {{ b }}", &vars).unwrap();
        assert_eq!(text, "3.10 007 hello null [broken");
    }

    #[test]
    fn boolean_words_are_recognised_in_any_case() {
        let vars = json!({"a": "True", "b": "FALSE", "c": "false"});
        let text = render(
            "{% if a %}a{% endif %}{% if b %}b{% endif %}{% if c %}c{% endif %}",
            &vars,
        )
        .unwrap();
        assert_eq!(text, "a");
    }

    #[test]
    fn an_undefined_read_names_the_line_and_the_unresolved_path_without_dumping_variables() {
        let vars = json!({"steps": {"done": {"secretish": "value-that-must-not-leak"}}});
        let err = render(
            "ok {{ steps.done.secretish | length }}\n• {{ steps.ghost.x }}",
            &vars,
        )
        .unwrap_err();
        assert!(err.contains("undefined value (in report:2)"), "{err}");
        assert!(err.contains("line: `• {{ steps.ghost.x }}`"), "{err}");
        assert!(err.ends_with("unresolved: steps.ghost.x"), "{err}");
        assert!(!err.contains("value-that-must-not-leak"), "{err}");
    }

    #[test]
    fn debug_cannot_render_the_whole_context() {
        assert!(render("{{ debug() }}", &json!({"token": "[[vault:agent:k]]"})).is_err());
    }

    #[test]
    fn testing_an_undefined_value_is_allowed_but_not_its_attributes() {
        let vars = json!({"steps": {}});
        let text = render("{% if steps.ghost %}a{% else %}b{% endif %}", &vars).unwrap();
        assert_eq!(text, "b");
        assert!(render("{% if steps.ghost.x %}a{% endif %}", &vars).is_err());
    }

    #[test]
    fn check_rejects_an_unclosed_tag_and_accepts_a_sound_template() {
        assert!(check("{% if x %}open").is_err());
        assert!(check("{{ ok }}").is_ok());
    }

    #[test]
    fn references_are_dotted_paths_without_loop_variables_filters_or_globals() {
        let refs = references(
            "{% for t in steps.ticket.items %}{{ t.key | upper }} {{ loop.index }}{% endfor %}\
             {{ range(3) | length }}{{ channel_id }}",
        );
        assert_eq!(
            refs,
            HashSet::from(["steps.ticket.items".to_string(), "channel_id".to_string()])
        );
        assert!(references("{% if").is_empty());
    }

    #[test]
    fn a_runaway_template_runs_out_of_fuel_instead_of_hanging() {
        let err = render(
            "{% for i in range(10000) %}{% for j in range(10000) %}x{% endfor %}{% endfor %}",
            &json!({}),
        )
        .unwrap_err();
        assert!(err.contains("fuel"), "{err}");
    }
}
